//! Capture: start a recording (warm reuse, always-on session or cold open),
//! cancel one, and the ring-buffer helpers that move audio into the ASR
//! backup and the recovery archive.
use super::*;

impl WorkerState {
    pub(super) fn on_start(&mut self, transform: bool) -> Flow {
        if self.session_active {
            return Flow::Now;
        }
        let pressed_at = Instant::now();
        // A new recording attempt invalidates the previously retained "last
        // recording" buffer, even if this attempt fails to open the mic: the
        // stale audio must never be replayable.
        self.last_audio.clear();
        // Warm-window reuse: a retained engine with idle IO paused is resumed
        // in place, skipping the stream rebuild cost for a back-to-back
        // dictation. Rebuild instead if the default input changed since the
        // stream was built.
        if self.engine.is_some() {
            if self.parked_until.is_none() {
                // An engine is already actively recording: swallow the
                // spurious re-press.
                return Flow::Now;
            }
            // Not only a device change: a stream that cpal has reported an
            // error on is dead while the object is alive, and `Instant` does
            // not advance while the Mac sleeps, so a lid closed overnight can
            // leave a "fresh" parked stream bound to a device that is gone.
            if let Some(why) = self.engine.as_mut().and_then(|e| e.should_rebuild()) {
                eprintln!("[audio] {why} → rebuilt warm stream");
                self.close_mic();
                // fall through to a fresh open below
            } else {
                let flag = match self.engine.as_mut().unwrap().resume() {
                    Ok(flag) => flag,
                    Err(error) => {
                        eprintln!("[audio] retained capture failed to resume: {error}; opening fresh");
                        self.close_mic();
                        self.restart_recording = Some(transform);
                        return Flow::Now;
                    }
                };
                if let Some(ring) = self.ring.as_mut() {
                    ring.recording_flag = flag.clone();
                }
                self.parked_until = None;
                self.arm_recording(pressed_at, transform, flag, "▶ recording (warm reuse)");
                return Flow::Now;
            }
        }
        // Always-on (pre-roll) path: the stream is already warm, so just open
        // a session. The next audio callback flushes the pre-roll lookback
        // (audio from *before* this press) into the consumer, then live audio,
        // so the first words can't be clipped.
        if self.always.is_some() {
            // The warm stream is bound to whatever input was default at boot.
            // If the user has since switched inputs (AirPods connected, a call
            // app grabbed the mic), rebuild it now so we capture from the
            // *current* device instead of silently recording the old one.
            // Same three tests as the warm stream, through the same function:
            // the always-on stream is open for hours, so it is the one most
            // likely to have been slept through.
            if let Some(why) = self.always.as_ref().and_then(|(ao, _)| ao.should_rebuild()) {
                let preroll = crate::audio::preroll_samples(self.config.capture.preroll_ms);
                let (mut ao2, cons2) = crate::audio::AlwaysOnCapture::new(preroll, BUFFER_CAPACITY);
                match ao2.start() {
                    Ok(()) => {
                        eprintln!(
                            "[audio] {why} → rebuilt always-on stream on `{}`",
                            ao2.device_name()
                        );
                        self.always = Some((ao2, cons2));
                    }
                    Err(e) => eprintln!(
                        "[warn] always-on rebuild failed ({e}); keeping previous stream"
                    ),
                }
            }
            if let Some((ao, _)) = self.always.as_ref() {
                ao.begin_session();
                let flag = ao.recording_flag();
                self.session_active = true;
                self.arm_recording(pressed_at, transform, flag, "▶ recording · pre-roll");
            }
            return Flow::Now;
        }
        // Fresh cold open (no parked engine to reuse).
        match self.open_mic() {
            Some(flag) => self.arm_recording(pressed_at, transform, flag, "▶ recording"),
            None => {
                cues::play_error();
                // The cue alone is not enough: it plays while the app's own
                // system mute is still held, and it says nothing.
                ui_channel::show_transient_if_unowned(UiState::Unavailable, FAILURE_PILL);
            }
        }
        Flow::Settle
    }

    /// Build a new engine and open the mic. `None` when it failed to open: a
    /// denied or missing microphone is the reason, and it is the one the user
    /// can act on, so raising it here means the menu bar, the pill and the
    /// Setup window all say the same thing.
    fn open_mic(&mut self) -> Option<Arc<AtomicBool>> {
        let (mut engine, consumer) = AudioCaptureEngine::new(BUFFER_CAPACITY);
        let flag = match engine.start_microphone() {
            Ok(flag) => flag,
            Err(err) => {
                eprintln!("[err]  mic open failed: {err:?}");
                crate::permissions::refresh_microphone_health();
                if !crate::health::is_raised(crate::health::Fault::MicrophoneBlocked) {
                    crate::health::raise(crate::health::Fault::AudioLost, format!("{err}"));
                }
                return None;
            }
        };
        self.engine = Some(engine);
        self.ring = Some(Ring { consumer, recording_flag: flag.clone() });
        Some(flag)
    }

    /// The mic is capturing: cue, mute other audio, arm the per-utterance
    /// state and start the key-press focus capture. Shared by every start
    /// path so a fast re-press gets identical setup.
    pub(super) fn arm_recording(&mut self, pressed_at: Instant, transform: bool, flag: Arc<AtomicBool>, line: &str) {
        cues::play_start();
        // Mute any other system audio for the duration of capture + processing;
        // restored on every release exit path via the RestoreOnDrop guard.
        crate::audio_duck::mute();
        eprintln!("{line}");
        crate::ipc::DAEMON_RECORDING.store(true, Ordering::SeqCst);
        self.last_audio.clear(); // a new recording invalidates the previous buffer
        let generation = begin_recording_feedback(self.config.capture.streaming_asr, flag);
        self.stream.reset(self.streaming_segmenter());
        self.noise_floor.clear();
        self.recording = Some(Recording { generation, pressed_at, transform, limit_hit: false });
        // The AX wake / cross-process cost is paid while the user is speaking.
        self.pending_target = Some(spawn_focus_capture());
    }

    /// External "cancel" (control socket, or the Escape key): stop capture and
    /// throw the utterance away, no transcribe, no inject. Mirrors the
    /// StopRecording teardown minus the pipeline.
    ///
    /// Stale-cancel guard: a `CancelRecording` queued from a PREVIOUS utterance
    /// (whose gate was already ended when it settled) must NOT discard a
    /// recording that started since. Only honour the cancel when the gate
    /// still has it pending for the CURRENT utterance; a fresh `begin()`
    /// clears the target, so an idle / stale event is a no-op and leaves the
    /// live recording alone.
    pub(super) fn on_cancel(&mut self) -> Flow {
        let Some(generation) = self
            .recording
            .as_ref()
            .map(|r| r.generation)
            .filter(|g| crate::cancel::CANCEL.requested(*g))
        else {
            return Flow::Now;
        };
        crate::ipc::DAEMON_RECORDING.store(false, Ordering::SeqCst);
        let mut on_demand_engine = self.engine.take();
        let always_on_session = std::mem::take(&mut self.session_active);
        if on_demand_engine.is_none() && !always_on_session {
            return Flow::Now;
        }
        // Un-mute other audio on every exit, exactly like StopRecording.
        let _unmute = crate::audio_duck::RestoreOnDrop;
        self.recording = None;
        self.pending_target = None; // discard the orphaned press-time capture
        if let Some(e) = on_demand_engine.as_mut() {
            if self.warm_window_active() {
                // Warm-window cancel: park the mic for reuse, but first drain
                // the cancelled audio and RETAIN it for retry (the next
                // recording clears this buffer on start).
                if let Err(error) = e.park() {
                    eprintln!("[audio] cancelled capture could not be retained: {error}");
                    e.stop_capture();
                }
                if let Some(ring) = self.ring.as_mut() {
                    retain_in_flight_audio(&mut self.last_audio, &self.stream.audio, Some(&mut ring.consumer), Some(&ring.recording_flag), Some(generation));
                }
                self.engine = on_demand_engine.take();
                self.parked_until = Some(Instant::now()
                    + Duration::from_millis(self.config.capture.warm_window_ms as u64));
            } else {
                e.stop_capture();
                // Cold cancel: drain + retain the partial recording before
                // dropping the ring below.
                if let Some(ring) = self.ring.as_mut() {
                    retain_in_flight_audio(&mut self.last_audio, &self.stream.audio, Some(&mut ring.consumer), Some(&ring.recording_flag), Some(generation));
                }
            }
        }
        if always_on_session {
            // End the session, drain the buffered audio, and RETAIN it for
            // retry (the next recording clears it on start).
            if let Some((ao, cons)) = self.always.as_mut() {
                ao.end_session();
                let recflag = ao.recording_flag();
                retain_in_flight_audio(&mut self.last_audio, &self.stream.audio, Some(cons), Some(&recflag), Some(generation));
            }
        } else if self.parked_until.is_none() {
            // Drop the per-utterance ring unread (the warm-park path keeps it
            // for the next reuse).
            self.ring = None;
        }
        self.stream.reset(None);
        cues::play_cancel();
        crate::history::finish_recovery(generation, crate::history::RecoveryStatus::Cancelled);
        ui_channel::finish_utterance(generation);
        eprintln!("⌫ cancelled · retained for recovery");
        eprintln!();
        Flow::Settle
    }
}

/// Pop everything currently sitting in the capture ring buffer (non-blocking).
/// Used by the streaming path to pull newly captured audio on each tick + at
/// release without waiting on the recording flag.
pub(super) fn drain_available(consumer: &mut crate::audio::HeapAudioConsumer) -> Vec<f32> {
    use ringbuf::traits::{Consumer, Observer};
    let n = consumer.occupied_len();
    if n == 0 {
        return Vec::new();
    }
    let mut buf = vec![0.0_f32; n];
    let got = consumer.pop_slice(&mut buf);
    buf.truncate(got);
    buf
}

/// Retain the in-flight recording's raw PCM as the "last recording" buffer for
/// retry, replacing whatever was there. Combines the already-drained streaming
/// backup (`stream_audio`) with the still-queued ring samples drained from
/// `consumer` (the caller must have stopped/parked capture first, so the
/// recording flag is false and `drain_session` returns). A cancel with no
/// captured audio yields an empty buffer — retry then no-ops. Memory only;
/// never written to disk.
pub(super) fn retain_in_flight_audio(
    last_audio: &mut Vec<f32>,
    stream_audio: &[f32],
    consumer: Option<&mut crate::audio::HeapAudioConsumer>,
    is_recording: Option<&std::sync::atomic::AtomicBool>,
    generation: Option<u64>,
) {
    let mut retained = Vec::with_capacity(stream_audio.len() + 4096);
    retained.extend_from_slice(stream_audio);
    if let (Some(c), Some(r)) = (consumer, is_recording) {
        let tail = crate::audio::drain_session(c, r);
        retained.extend_from_slice(&tail);
        if let Some(gen) = generation {
            crate::history::append_recovery_audio(gen, tail, crate::audio::SAMPLE_RATE);
        }
    }
    *last_audio = retained;
}


/// Spawn a background thread that resolves the currently-focused UI element +
/// caret context and returns a receiver for its result. Called at key-PRESS so
/// the AX wake / cross-process cost is paid while the user is speaking, and the
/// result is joined (usually instantly) at release. A very short hold simply
/// blocks on the join — the same cost the old release-time spawn paid, and
/// usually strictly less because the resolve happened during speech, not after.
///
/// The app identity (PID + bundle id) is read first and travels with the
/// result even when the AX capture fails: terminals and other AX-blind apps
/// expose no focused element, and they are exactly the apps whose identity
/// decides raw-vs-cleaned text.
pub(super) fn spawn_focus_capture() -> std::sync::mpsc::Receiver<PressCapture> {
    let (tx, rx) = std::sync::mpsc::channel::<PressCapture>();
    std::thread::spawn(move || {
        let app = crate::injector::frontmost_target_app();
        let _ = tx.send((app, FocusTarget::capture()));
    });
    rx
}

/// What the press-time capture thread hands back: the app, and its focus.
pub(super) type PressCapture = (Option<crate::injector::TargetApp>, eyre::Result<FocusTarget>);

/// Queue ownership of captured samples to the archive after retaining the ASR
/// backup. No disk I/O runs here or on the audio callback.
pub(super) fn checkpoint_capture(consumer: &mut crate::audio::HeapAudioConsumer, backup: &mut Vec<f32>, generation: u64) {
    checkpoint_capture_with(consumer, backup, |_| {}, |samples| {
        crate::history::append_recovery_audio(generation, samples, crate::audio::SAMPLE_RATE);
    });
}

/// Shared by streaming, whole-buffer, and pre-roll capture. The observer runs
/// before ownership of the samples is handed to the archive queue.
pub(super) fn checkpoint_capture_with(consumer: &mut crate::audio::HeapAudioConsumer, backup: &mut Vec<f32>,
    mut observe: impl FnMut(&[f32]), archive: impl FnOnce(Vec<f32>)) {
    let samples = drain_available(consumer);
    if samples.is_empty() { return; }
    observe(&samples);
    backup.extend_from_slice(&samples);
    archive(samples);
}

pub(super) fn complete_capture(consumer: &mut crate::audio::HeapAudioConsumer,
    recording: &std::sync::atomic::AtomicBool, backup: &mut Vec<f32>, generation: u64) -> Vec<f32> {
    complete_capture_with(consumer, recording, backup, |tail| {
        crate::history::append_recovery_audio(generation, tail, crate::audio::SAMPLE_RATE);
    })
}

fn complete_capture_with(consumer: &mut crate::audio::HeapAudioConsumer,
    recording: &std::sync::atomic::AtomicBool, backup: &mut Vec<f32>, archive: impl FnOnce(Vec<f32>)) -> Vec<f32> {
    let tail = crate::audio::drain_session(consumer, recording);
    backup.extend_from_slice(&tail);
    if !tail.is_empty() { archive(tail); }
    std::mem::take(backup)
}

/// Register the capture flag with its generation (so Cancel stops the
/// producer even inside a synchronous model call) and open its History record.
pub(super) fn begin_recording_feedback(preview: bool, capture: Arc<AtomicBool>) -> u64 {
    let generation = ui_channel::begin_capturing(preview, capture);
    crate::history::begin_recovery(generation);
    generation
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoints_and_final_tail_keep_every_sample_once_in_each_capture_mode() {
        use ringbuf::{HeapRb, traits::{Split, Producer}};
        for streaming in [false, true] {
            for preroll in [false, true] {
                let (mut producer, mut consumer) = HeapRb::<f32>::new(32).split();
                let mut backup = Vec::new();
                let mut archived = Vec::new();
                let mut segmenter = crate::vad::SegmentStream::new(crate::audio::SAMPLE_RATE, vad_config(false));
                let mut input = Vec::new();
                for chunk in [vec![0.1, 0.2], vec![0.3, 0.4]] {
                    input.extend_from_slice(&chunk);
                    producer.push_slice(&chunk);
                    checkpoint_capture_with(&mut consumer, &mut backup, |samples| {
                        if streaming && !preroll { segmenter.push(samples); }
                    }, |samples| archived.extend(samples));
                }
                let tail = [0.5, 0.6];
                input.extend_from_slice(&tail);
                producer.push_slice(&tail);
                let audio = complete_capture_with(&mut consumer, &AtomicBool::new(false), &mut backup,
                    |samples| archived.extend(samples));
                assert_eq!(audio, input, "ASR lost/duplicated samples, streaming={streaming} preroll={preroll}");
                assert_eq!(archived, input, "archive lost/duplicated samples");
                assert!(backup.is_empty(), "next recording must not inherit checkpoint audio");
                assert!(drain_available(&mut consumer).is_empty());
            }
        }
    }

    #[test]
    fn cancel_retains_checkpoint_audio_and_undrained_tail_without_previous_recording() {
        use ringbuf::{HeapRb, traits::{Split, Producer}};
        let (mut producer, mut consumer) = HeapRb::<f32>::new(16).split();
        producer.push_slice(&[0.3, 0.4]);
        let mut last = vec![9.0];
        retain_in_flight_audio(&mut last, &[0.1, 0.2], Some(&mut consumer), Some(&AtomicBool::new(false)), None);
        assert_eq!(last, vec![0.1, 0.2, 0.3, 0.4]);
        assert!(drain_available(&mut consumer).is_empty());
    }

    /// Synthetic-audio fixture: the already-drained streaming backup (what the
    /// hold-time ticks moved into `stream_audio`) plus a drained ring tail.
    /// `retain_in_flight_audio` must concatenate both and REPLACE the previous
    /// buffer — no history/log reads, no disk, memory only.
    #[test]
    fn retain_keeps_stream_backup_and_replaces_previous_buffer() {
        let mut last = vec![9.0, 9.0, 9.0];
        let stream_audio = vec![1.0, 2.0, 3.0];
        // No live consumer: only the already-drained backup is retained.
        retain_in_flight_audio(&mut last, &stream_audio, None, None, None);
        assert_eq!(last, vec![1.0, 2.0, 3.0], "must replace, not append, the old buffer");
    }

    #[test]
    fn retain_with_no_audio_yields_empty_so_retry_noops() {
        let mut last = vec![5.0];
        retain_in_flight_audio(&mut last, &[], None, None, None);
        assert!(last.is_empty(), "a cancel with nothing captured must not leave a stale buffer");
    }

    #[test]
    fn retain_empty_does_not_keep_zero_length_invalid_retry() {
        // A retained buffer is only meaningful when it holds samples; the
        // worker's retry path no-ops on an empty buffer.
        let mut last = Vec::new();
        retain_in_flight_audio(&mut last, &[], None, None, None);
        assert!(last.is_empty());
    }
}
