use super::*;
use std::cell::{Cell, RefCell};

/// A stand-in for `cpal::Stream` that records lifecycle calls and can be
/// told to refuse either one.
#[derive(Default)]
struct FakeStream {
    playing: Cell<bool>,
    fail_play: Cell<bool>,
    fail_pause: Cell<bool>,
    calls: RefCell<Vec<&'static str>>,
}

impl AudioStreamControl for FakeStream {
    fn start_io(&self) -> Result<(), String> {
        self.calls.borrow_mut().push("play");
        if self.fail_play.get() {
            return Err("play refused".into());
        }
        self.playing.set(true);
        Ok(())
    }
    fn stop_io(&self) -> Result<(), String> {
        self.calls.borrow_mut().push("pause");
        if self.fail_pause.get() {
            return Err("pause refused".into());
        }
        self.playing.set(false);
        Ok(())
    }
}

fn fresh_flags() -> (AtomicBool, AtomicBool, AtomicU64) {
    (
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicU64::new(0),
    )
}

#[test]
fn pause_clears_recording_before_it_stops_the_stream() {
    let stream = FakeStream::default();
    let (recording, failed, _epoch) = fresh_flags();
    recording.store(true, Ordering::SeqCst);
    pause_retained(&stream, &recording, &failed).unwrap();
    assert!(!recording.load(Ordering::SeqCst));
    assert!(!failed.load(Ordering::SeqCst));
    assert!(!stream.playing.get());
    assert_eq!(&*stream.calls.borrow(), &["pause"]);
}

#[test]
fn a_refused_pause_clears_recording_and_marks_the_stream_for_rebuild() {
    let _serial = crate::health::test_lock();
    let stream = FakeStream {
        fail_pause: Cell::new(true),
        ..FakeStream::default()
    };
    let (recording, failed, _epoch) = fresh_flags();
    recording.store(true, Ordering::SeqCst);
    let err = pause_retained(&stream, &recording, &failed).unwrap_err();
    assert!(err.to_string().contains("pause"), "{err}");
    assert!(
        !recording.load(Ordering::SeqCst),
        "a refused pause must not leave the engine capturing"
    );
    assert!(
        failed.load(Ordering::SeqCst),
        "the caller must rebuild, not trust a stream it could not pause"
    );
    assert!(crate::health::is_raised(crate::health::Fault::AudioLost));
    crate::health::clear(crate::health::Fault::AudioLost);
}

#[test]
fn resume_claims_recording_only_after_the_stream_really_plays() {
    let stream = FakeStream::default();
    let (recording, failed, epoch) = fresh_flags();
    resume_retained(&stream, &recording, &failed, &epoch).unwrap();
    assert!(stream.playing.get());
    assert!(recording.load(Ordering::SeqCst));
    assert!(!failed.load(Ordering::SeqCst));
    assert_eq!(epoch.load(Ordering::SeqCst), 1);
}

#[test]
fn a_refused_resume_never_claims_recording_and_marks_the_stream_for_rebuild() {
    let _serial = crate::health::test_lock();
    let stream = FakeStream {
        fail_play: Cell::new(true),
        ..FakeStream::default()
    };
    let (recording, failed, epoch) = fresh_flags();
    let err = resume_retained(&stream, &recording, &failed, &epoch).unwrap_err();
    assert!(err.to_string().contains("play"), "{err}");
    assert!(
        !recording.load(Ordering::SeqCst),
        "a failed resume must not look like recording"
    );
    assert!(failed.load(Ordering::SeqCst));
    assert!(!stream.playing.get());
    assert!(crate::health::is_raised(crate::health::Fault::AudioLost));
    crate::health::clear(crate::health::Fault::AudioLost);
}

/// The epoch must be bumped *before* `play`, and the flag set only *after*:
/// that is what lets the callback's recording-first/epoch-second load order
/// reset the carry before it emits the first sample of the new utterance.
#[test]
fn resume_signals_the_new_utterance_epoch_before_it_starts_the_stream() {
    struct OrderStream<'a> {
        recording: &'a AtomicBool,
        epoch: &'a AtomicU64,
        recording_at_play: Cell<bool>,
        epoch_at_play: Cell<u64>,
    }
    impl AudioStreamControl for OrderStream<'_> {
        fn start_io(&self) -> Result<(), String> {
            self.recording_at_play
                .set(self.recording.load(Ordering::SeqCst));
            self.epoch_at_play.set(self.epoch.load(Ordering::SeqCst));
            Ok(())
        }
        fn stop_io(&self) -> Result<(), String> {
            Ok(())
        }
    }

    let recording = AtomicBool::new(false);
    let failed = AtomicBool::new(false);
    let epoch = AtomicU64::new(0);
    let stream = OrderStream {
        recording: &recording,
        epoch: &epoch,
        recording_at_play: Cell::new(true), // sentinel: must be overwritten
        epoch_at_play: Cell::new(u64::MAX),
    };
    resume_retained(&stream, &recording, &failed, &epoch).unwrap();
    assert!(
        !stream.recording_at_play.get(),
        "recording must not be claimed until play has returned"
    );
    assert_eq!(
        stream.epoch_at_play.get(),
        1,
        "epoch must be bumped before play so the callback resets in time"
    );
    assert!(recording.load(Ordering::SeqCst));
}

#[test]
fn a_resumed_utterance_drops_the_previous_carry_instead_of_interpolating_it() {
    let (mut producer, mut consumer) = HeapRb::<f32>::new(64).split();
    let recording = AtomicBool::new(false);
    let epoch = AtomicU64::new(0);
    let mut state = ResampleState::new(1, SAMPLE_RATE, 0);

    // First utterance ends loud, leaving `last == 0.9`.
    recording.store(true, Ordering::SeqCst);
    push_resampled(&[0.5, 0.9], 1, 1.0, &mut state, &epoch, &mut producer, &recording);

    // Park, then resume the same retained stream.
    recording.store(false, Ordering::SeqCst);
    epoch.fetch_add(1, Ordering::SeqCst);
    recording.store(true, Ordering::SeqCst);
    push_resampled(&[0.1, 0.1], 1, 1.0, &mut state, &epoch, &mut producer, &recording);

    let mut out = vec![0.0f32; consumer.occupied_len()];
    consumer.pop_slice(&mut out);
    // The very first callback's first sample uses the initial carry (0.0).
    assert_eq!(&out[..2], &[0.0, 0.5], "first utterance");
    // The new utterance resets the carry to 0.0; without the reset this
    // sample would be the previous utterance's 0.9 — a click at the seam.
    assert_eq!(out[2], 0.0, "stale carry leaked into the resumed utterance");
    assert_eq!(out[3], 0.1);
}

#[test]
fn a_new_utterance_clears_an_undecided_probe_but_keeps_a_chosen_channel() {
    // Undecided 8-channel picker: reset must drop the partial probe energy.
    let rate = 48_000u32;
    let channels = 8usize;
    let mut picker = ChannelPicker::new(channels, rate);
    let probe = picker.probe_frames;
    let mut partial = vec![0.0f32; channels * 10];
    for frame in partial.chunks_exact_mut(channels) {
        frame[3] = 0.4;
    }
    let _ = picker.observe(&partial);
    picker.reset_for_new_utterance();
    assert_eq!(
        picker.observe(&vec![0.0001f32; channels * probe]),
        Downmix::Mean,
        "partial probe energy must not survive into the new utterance"
    );

    // Decided picker: the chosen channel is kept (a device change rebuilds).
    let mut decided = ChannelPicker::new(channels, rate);
    let probe = decided.probe_frames;
    let mut speech = vec![0.0f32; channels * probe];
    for frame in speech.chunks_exact_mut(channels) {
        frame[5] = 0.4;
    }
    assert_eq!(decided.observe(&speech), Downmix::Channel(5));
    decided.reset_for_new_utterance();
    assert_eq!(
        decided.observe(&vec![0.0001f32; channels * probe]),
        Downmix::Channel(5)
    );
}
